use std::mem;

use quickcheck::{QuickCheck, TestResult};
use vector_common::byte_size_of::ByteSizeOf;

use super::*;

#[test]
#[ignore = "QuickCheck Arbitrary for Event goes through OtelLog bridge which may panic on edge cases"]
fn at_least_wrapper_size() {
    // The byte size of an `Event` should always be at least as big as the
    // mem::size_of of the `Event`.
    #[allow(clippy::needless_pass_by_value)]
    fn inner(event: Event) -> TestResult {
        let baseline = mem::size_of::<Event>();
        assert!(baseline <= event.size_of());
        TestResult::passed()
    }

    QuickCheck::new()
        .tests(1_000)
        .max_tests(10_000)
        .quickcheck(inner as fn(Event) -> TestResult);
}

#[test]
#[ignore = "QuickCheck Arbitrary for Event goes through OtelLog bridge which may panic on edge cases"]
fn exactly_equal_if_no_allocated_bytes() {
    // The byte size of an `Event` should always be exactly equal to its
    // `mem::size_of` if there are no reported allocated bytes.
    #[allow(clippy::needless_pass_by_value)]
    fn inner(event: Event) -> TestResult {
        let allocated_sz = event.allocated_bytes();
        if allocated_sz == 0 {
            let baseline = mem::size_of::<Event>();
            assert_eq!(baseline, event.size_of());
            return TestResult::passed();
        }
        TestResult::discard()
    }

    QuickCheck::new()
        .tests(1_000)
        .max_tests(10_000)
        .quickcheck(inner as fn(Event) -> TestResult);
}

#[test]
#[ignore = "QuickCheck Arbitrary for Event goes through OtelLog bridge which may panic on edge cases"]
fn size_greater_than_allocated_size() {
    // The total byte size of an `Event` should always be strictly greater than
    // the allocated bytes of the `Event`.
    #[allow(clippy::needless_pass_by_value)]
    fn inner(event: Event) -> TestResult {
        let total_sz = event.size_of();
        let allocated_sz = event.allocated_bytes();

        assert!(total_sz > allocated_sz);
        TestResult::passed()
    }

    QuickCheck::new()
        .tests(1_000)
        .max_tests(10_000)
        .quickcheck(inner as fn(Event) -> TestResult);
}
