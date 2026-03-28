/// Arbor integration tests — workspace lifecycle + fork/reseal.
///
/// These tests run against the controller and DB layer directly
/// without starting a real Firecracker VM. The runner-agent calls
/// are mocked via a tiny HTTP server that returns success responses.
///
/// Run with:
///   DATABASE_URL=postgresql://arbor:pass@localhost/arbor_test \
///   cargo test --test integration -- --nocapture
mod db_tests;
mod reseal_tests;
mod api_smoke;
