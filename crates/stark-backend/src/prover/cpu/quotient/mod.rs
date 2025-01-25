use std::sync::Arc;

use itertools::{izip, multiunzip, Itertools};
use p3_commit::{Pcs, PolynomialSpace};
use p3_field::FieldAlgebra;
use p3_matrix::{dense::RowMajorMatrix, Matrix};
use p3_util::log2_strict_usize;
use tracing::instrument;

use self::single::compute_single_rap_quotient_values;
use super::PcsData;
use crate::{
    air_builders::symbolic::SymbolicExpressionDag,
    config::{Com, Domain, PackedChallenge, StarkGenericConfig, Val},
    prover::types::RapView,
};

mod evaluator;
pub(crate) mod single;

pub struct QuotientCommitter<'pcs, SC: StarkGenericConfig> {
    pcs: &'pcs SC::Pcs,
    alpha: SC::Challenge,
}

impl<'pcs, SC: StarkGenericConfig> QuotientCommitter<'pcs, SC> {
    pub fn new(pcs: &'pcs SC::Pcs, alpha: SC::Challenge) -> Self {
        Self { pcs, alpha }
    }

    /// Constructs quotient domains and computes the evaluation of the quotient polynomials
    /// on the quotient domains of each RAP.
    ///
    /// ## Assumptions
    /// - `constraints`, `extended_views`, `quotient_degrees` have equal lengths and the length equals number of RAPs.
    /// - `quotient_degrees` is the factor to **multiply** the trace degree by to get the degree of the quotient polynomial. This should be determined from the constraint degree of the RAP.
    /// - `extended_views` is a view of the trace polynomials evaluated on the quotient domain, with rows bit reversed to account for the fact that the quotient domain is different for each RAP.
    #[instrument(name = "compute quotient values", level = "info", skip_all)]
    pub fn quotient_values(
        &self,
        constraints: &[&SymbolicExpressionDag<Val<SC>>],
        extended_views: Vec<RapView<impl Matrix<Val<SC>>, Val<SC>, SC::Challenge>>,
        quotient_degrees: &[u8],
    ) -> QuotientData<SC> {
        assert_eq!(constraints.len(), extended_views.len());
        assert_eq!(constraints.len(), quotient_degrees.len());
        let inner = izip!(constraints, extended_views, quotient_degrees)
            .map(|(constraints, extended_view, &quotient_degree)| {
                self.single_rap_quotient_values(constraints, extended_view, quotient_degree)
            })
            .collect();
        QuotientData { inner }
    }

    pub(super) fn single_rap_quotient_values(
        &self,
        constraints: &SymbolicExpressionDag<Val<SC>>,
        view: RapView<impl Matrix<Val<SC>>, Val<SC>, SC::Challenge>,
        quotient_degree: u8,
    ) -> SingleQuotientData<SC> {
        let log_trace_height = view.pair.log_trace_height;
        let trace_domain = self
            .pcs
            .natural_domain_for_degree(1usize << log_trace_height);
        let quotient_domain =
            trace_domain.create_disjoint_domain(trace_domain.size() * quotient_degree as usize);

        let (after_challenge_lde_on_quotient_domain, challenges, exposed_values_after_challenge): (
            Vec<_>,
            Vec<_>,
            Vec<_>,
        ) = multiunzip(view.per_phase.into_iter().map(|view| {
            (
                view.inner
                    .expect("gap in challenge phase not supported yet"),
                view.challenges
                    .into_iter()
                    .map(PackedChallenge::<SC>::from_f)
                    .collect_vec(),
                view.exposed_values
                    .into_iter()
                    .map(PackedChallenge::<SC>::from_f)
                    .collect_vec(),
            )
        }));

        let quotient_values = compute_single_rap_quotient_values::<SC, _>(
            constraints,
            trace_domain,
            quotient_domain,
            view.pair.preprocessed,
            view.pair.partitioned_main,
            after_challenge_lde_on_quotient_domain,
            &challenges,
            self.alpha,
            &view.pair.public_values,
            &exposed_values_after_challenge,
        );
        SingleQuotientData {
            quotient_degree: quotient_degree as usize,
            quotient_domain,
            quotient_values,
        }
    }

    #[instrument(name = "commit to quotient poly chunks", skip_all)]
    pub fn commit(&self, data: QuotientData<SC>) -> (Com<SC>, PcsData<SC>) {
        let (log_trace_heights, quotient_domains_and_chunks): (Vec<_>, Vec<_>) = data
            .split()
            .into_iter()
            .map(|q| {
                (
                    log2_strict_usize(q.domain.size()) as u8,
                    (q.domain, q.chunk),
                )
            })
            .unzip();
        let (commit, data) = self.pcs.commit(quotient_domains_and_chunks);
        (
            commit,
            PcsData {
                data: Arc::new(data),
                log_trace_heights,
            },
        )
    }
}

/// The quotient polynomials from multiple RAP matrices.
pub(super) struct QuotientData<SC: StarkGenericConfig> {
    inner: Vec<SingleQuotientData<SC>>,
}

impl<SC: StarkGenericConfig> QuotientData<SC> {
    /// Splits the quotient polynomials from multiple AIRs into chunks of size equal to the trace domain size.
    pub fn split(self) -> impl IntoIterator<Item = QuotientChunk<SC>> {
        self.inner.into_iter().flat_map(|data| data.split())
    }
}

/// The quotient polynomial from a single matrix RAP, evaluated on the quotient domain.
pub(super) struct SingleQuotientData<SC: StarkGenericConfig> {
    quotient_degree: usize,
    /// Quotient domain
    quotient_domain: Domain<SC>,
    /// Evaluations of the quotient polynomial on the quotient domain
    quotient_values: Vec<SC::Challenge>,
}

impl<SC: StarkGenericConfig> SingleQuotientData<SC> {
    /// The vector of evaluations of the quotient polynomial on the quotient domain,
    /// first flattened from vector of extension field elements to matrix of base field elements,
    /// and then split into chunks of size equal to the trace domain size (quotient domain size
    /// divided by `quotient_degree`).
    pub fn split(self) -> impl IntoIterator<Item = QuotientChunk<SC>> {
        let quotient_degree = self.quotient_degree;
        let quotient_domain = self.quotient_domain;
        // Flatten from extension field elements to base field elements
        let quotient_flat = RowMajorMatrix::new_col(self.quotient_values).flatten_to_base();
        let quotient_chunks = quotient_domain.split_evals(quotient_degree, quotient_flat);
        let qc_domains = quotient_domain.split_domains(quotient_degree);
        qc_domains
            .into_iter()
            .zip_eq(quotient_chunks)
            .map(|(domain, chunk)| QuotientChunk { domain, chunk })
    }
}

/// The vector of evaluations of the quotient polynomial on the quotient domain,
/// split into chunks of size equal to the trace domain size (quotient domain size
/// divided by `quotient_degree`).
///
/// This represents a single chunk, where the vector of extension field elements is
/// further flattened to a matrix of base field elements.
pub struct QuotientChunk<SC: StarkGenericConfig> {
    /// Chunk of quotient domain, which is a coset of the trace domain
    pub domain: Domain<SC>,
    /// Matrix with number of rows equal to trace domain size,
    /// and number of columns equal to extension field degree.
    pub chunk: RowMajorMatrix<Val<SC>>,
}
