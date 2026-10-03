pub(crate) const HANDOFF_BOUND: usize = 8;

#[derive(Clone, Debug)]
pub(crate) enum Selection {
    FollowDefault,
    Explicit { id: String, name: String },
}

pub(crate) struct HandoffRequest<PayloadT, InfoT> {
    pub(crate) payload: PayloadT,
    pub(crate) info: InfoT,
    pub(crate) generation: u64,
}
