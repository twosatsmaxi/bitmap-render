use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};
use std::sync::Arc;

pub fn test() {
    let conf = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(2)
            .burst_size(10)
            .finish()
            .unwrap(),
    );
    let layer = GovernorLayer { config: conf };
}
