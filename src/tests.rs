use super::*;

#[test]
fn the_log_directory_stays_where_the_shipping_build_wrote_it() {
    let Some(dir) = logging::log_dir() else {
        return;
    };
    assert!(dir.ends_with(r"com.liveback.desktop\logs"), "{dir:?}");
}
