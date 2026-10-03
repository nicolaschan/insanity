pub(crate) const HANDOFF_BOUND: usize = 8;

pub(crate) struct HandoffRequest<PayloadT, InfoT> {
    pub(crate) payload: PayloadT,
    pub(crate) info: InfoT,
    pub(crate) generation: u64,
}
