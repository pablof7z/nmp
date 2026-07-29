use std::time::{Duration, Instant};

use nmp::nmp_threads_live;
use nmp_bdd::NmpWorld;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_each_bdd_world_releases_its_engine_threads() {
    let baseline = nmp_threads_live();

    for iteration in 1..=3 {
        let mut world = NmpWorld::default();
        world.ensure_started().await;
        assert!(
            nmp_threads_live() > baseline,
            "a started BDD world must own instrumented engine threads"
        );

        drop(world);

        let deadline = Instant::now() + Duration::from_secs(2);
        while nmp_threads_live() != baseline && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            nmp_threads_live(),
            baseline,
            "BDD world {iteration} leaked NMP-owned engine threads after drop"
        );
    }
}
