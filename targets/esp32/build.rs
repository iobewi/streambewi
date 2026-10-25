fn main() {
    println!("cargo:rustc-link-arg-bins=-Tlinkall.x");
    // `status_led`: a board feature was chosen, so the target drives the status LED.
    println!("cargo::rustc-check-cfg=cfg(status_led)");
    let boards = [
        "CARGO_FEATURE_BOARD_DEVKITC_1",
        "CARGO_FEATURE_BOARD_DEVKITC_1_V1_1",
        "CARGO_FEATURE_BOARD_S3_ZERO",
    ];
    if boards.iter().any(|b| std::env::var_os(b).is_some()) {
        println!("cargo:rustc-cfg=status_led");
    }
}
