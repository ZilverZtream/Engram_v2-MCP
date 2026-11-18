//! One focused executable for the related natural-language delivery contracts.
//! Individual targets remain available for exact reruns; avoid repeated linking
//! when both contracts require the same source build.
#[path = "ask_authored_evidence_tests.rs"]
mod authored;
#[path = "ask_provider_availability_tests.rs"]
mod availability;
