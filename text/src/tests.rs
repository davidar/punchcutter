/// SPIR-V's magic number, little-endian — enough to prove the build script
/// baked a module rather than a diagnostic.
#[test]
fn the_demo_shader_is_baked() {
    assert!(super::DEMO_SPV.len() > 256);
    assert_eq!(&super::DEMO_SPV[..4], &[0x03, 0x02, 0x23, 0x07]);
}
