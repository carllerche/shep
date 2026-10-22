//! Attribute misuse is a compile error, with a message saying why.

#[test]
fn derive_errors() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
