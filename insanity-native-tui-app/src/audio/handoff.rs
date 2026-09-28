pub(crate) const HANDOFF_BOUND: usize = 8;

#[derive(Clone, Debug)]
pub enum Selection {
    FollowDefault,
    Explicit { id: String, name: String },
}

pub struct HandoffRequest<PayloadT, InfoT> {
    pub payload: PayloadT,
    pub info: InfoT,
    pub generation: u64,
}
