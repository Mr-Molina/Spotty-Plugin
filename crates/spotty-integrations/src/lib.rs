//! LMS integrations: DSTM, `MaterialSkin` `HomeExtras`, and `LastMix`.

pub mod dstm;
pub mod home_extras;
pub mod last_mix;

pub use crate::dstm::{
    DstmRecommender, SeedTrackInput, DSTM_HANDLER_ID, DSTM_RECOMMENDATION_LIMIT,
};
pub use crate::home_extras::{resolve_home_extra_menu, HomeExtraDescriptor, HOME_EXTRAS};
pub use crate::last_mix::{LastMixCandidate, LastMixService, LASTMIX_PROTOCOL};
