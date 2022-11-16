use candid::{CandidType, Deserialize};
use dfn_core::api::{call, CanisterId};
use rand::Rng;
use rand_chacha::ChaCha20Rng;
use rust_decimal::Decimal;
use serde::Serialize;

use std::convert::TryInto;
use std::fmt;
use std::fmt::Formatter;

use ic_base_types::PrincipalId;
use ic_ic00_types::{CanisterIdRecord, CanisterStatusResultV2, IC_00};

use url::{Host, Url};

pub mod ledger;
pub mod stable_mem_utils;

// 10^8
pub const E8: u64 = 100_000_000;

pub const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

// Useful as a piece of realistic test data.
pub const START_OF_2022_TIMESTAMP_SECONDS: u64 = 1641016800;

// The size of a WASM page in bytes, as defined by the WASM specification
#[cfg(any(target_arch = "wasm32"))]
const WASM_PAGE_SIZE_BYTES: usize = 65536;

#[macro_export]
macro_rules! assert_is_ok {
    ($result: expr) => {
        let r = $result;
        assert!(
            r.is_ok(),
            "result ({}) = {:#?}, not Ok",
            stringify!($result),
            r
        );
    };
}

#[macro_export]
macro_rules! assert_is_err {
    ($result: expr) => {
        let r = $result;
        assert!(
            r.is_err(),
            "result ({}) = {:#?}, not Err",
            stringify!($result),
            r
        );
    };
}

pub fn i2d(i: u64) -> Decimal {
    // Convert to i64.
    let i = i
        .try_into()
        .unwrap_or_else(|err| panic!("{} does not fit into i64: {:#?}", i, err));

    Decimal::new(i, 0)
}

/// A general purpose error indicating something went wrong.
#[derive(Default)]
pub struct NervousSystemError {
    pub error_message: String,
}

impl NervousSystemError {
    pub fn new() -> Self {
        NervousSystemError {
            ..Default::default()
        }
    }

    pub fn new_with_message(message: impl ToString) -> Self {
        NervousSystemError {
            error_message: message.to_string(),
        }
    }
}

impl fmt::Display for NervousSystemError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.error_message)
    }
}

impl fmt::Debug for NervousSystemError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.error_message)
    }
}

/// Description of a change to the authz of a specific method on a specific
/// canister that must happen for a given canister change/add/remove
/// to be viable
#[derive(CandidType, Serialize, Deserialize, Clone, Debug)]
pub struct MethodAuthzChange {
    pub canister: CanisterId,
    pub method_name: String,
    pub principal: Option<PrincipalId>,
    pub operation: AuthzChangeOp,
}

/// The operation to execute. Variable names in comments refer to the fields
/// of AuthzChange.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug)]
pub enum AuthzChangeOp {
    /// 'canister' must add a principal to the authorized list of 'method_name'.
    /// If 'add_self' is true, the canister_id to be authorized is the canister
    /// being added/changed, if it's false, 'principal' is used instead, which
    /// must be Some in that case..
    Authorize { add_self: bool },
    /// 'canister' must remove 'principal' from the authorized list of
    /// 'method_name'. 'principal' must always be Some.
    Deauthorize,
}

/// Return the status of the given canister. The caller must control the given canister.
pub async fn get_canister_status(
    canister_id: PrincipalId,
) -> Result<CanisterStatusResultV2, (Option<i32>, String)> {
    let canister_id_record: CanisterIdRecord = CanisterId::new(canister_id).unwrap().into();

    call(
        IC_00,
        "canister_status",
        dfn_candid::candid,
        (canister_id_record,),
    )
    .await
}

/// Generate a set of increasing dissolve delays where each new dissolve delay is randomized
/// within its interval.
///
/// Expressed another way, this method will create a vector of integers where the first element
/// is 0, the second element will be between [D, 2 * D), the third element will be between
/// [2 * D, 3 * D) and so on, where D = dissolve_delay_interval_seconds.
///
/// Preconditions:
///    - `interval_count` must be greater than 0
///    - `dissolve_delay_interval_seconds` must be greater than 0
pub fn generate_random_dissolve_delay_intervals(
    interval_count: u64,
    dissolve_delay_interval_seconds: u64,
    random_number_generator: &mut ChaCha20Rng,
) -> Vec<u64> {
    assert!(interval_count > 0);
    assert!(dissolve_delay_interval_seconds > 0);
    (0..interval_count)
        .map(|interval_i| {
            // The 0th interval should always be 0 so the dissolve delay of a neuron will
            // result in liquid tokens
            if interval_i == 0 {
                return 0;
            }

            let random_offset_within_one_interval =
                random_number_generator.gen_range(0..dissolve_delay_interval_seconds);

            interval_i
                .saturating_mul(dissolve_delay_interval_seconds)
                .saturating_add(random_offset_within_one_interval)
        })
        .collect()
}

#[cfg(test)]
mod test {
    use super::*;
    use proptest::prelude::proptest;
    use rand::SeedableRng;
    use rand_chacha::ChaCha20Rng;

    const TEN_YEARS_IN_SECONDS: u64 = 315360000;

    proptest! {
        // Test that the generate_random_dissolve_delay_intervals method returns increasing intervals
        // of dissolve delay
        #[test]
        fn test_generate_random_dissolve_delay_intervals(
            interval_count in 1_u64..48,
            dissolve_delay_interval_seconds in 1_u64..TEN_YEARS_IN_SECONDS,
            rng_seed in 0_u64..u64::MAX,
        ) {
            let mut rng = ChaCha20Rng::seed_from_u64(rng_seed);

            let random_dissolve_delay_intervals = generate_random_dissolve_delay_intervals(
                interval_count,
                dissolve_delay_interval_seconds,
                &mut rng
            );

            // The elements should be ordered and increasing
            let mut sorted = random_dissolve_delay_intervals.clone();
            sorted.sort_unstable();
            assert_eq!(random_dissolve_delay_intervals, sorted);

            // The first element should always be 0, and is a special case in the algorithm
            let first_element = random_dissolve_delay_intervals[0];
            assert_eq!(first_element, 0_u64);

            random_dissolve_delay_intervals
                .iter()
                .enumerate()
                .for_each(|(interval, dissolve_delay)| {
                    let interval_start = dissolve_delay_interval_seconds * interval as u64;
                    let interval_end = interval_start + dissolve_delay_interval_seconds;

                    let interval = interval_start..interval_end;
                    assert!(
                        interval.contains(dissolve_delay),
                        "Randomized dissolve delay {} not in expected interval {:?}",
                        dissolve_delay, interval
                    );
                })
        }
    }

    // Test that the edge case values of generate_random_dissolve_delay_intervals
    #[test]
    fn test_generate_random_dissolve_delay_intervals_with_edge_values() {
        let mut rng = ChaCha20Rng::seed_from_u64(1_u64);

        // The edge case values. These are the lowest possible values the
        // algorithm accepts
        let interval_count = 1;
        let dissolve_delay_interval_seconds = 1;

        let random_dissolve_delay_intervals = generate_random_dissolve_delay_intervals(
            interval_count,
            dissolve_delay_interval_seconds,
            &mut rng,
        );

        // The elements should be ordered and increasing
        let mut sorted = random_dissolve_delay_intervals.clone();
        sorted.sort_unstable();
        assert_eq!(random_dissolve_delay_intervals, sorted);

        // The first element should always be 0, and is a special case in the algorithm
        let first_element = random_dissolve_delay_intervals[0];
        assert_eq!(first_element, 0_u64);

        random_dissolve_delay_intervals
            .iter()
            .enumerate()
            .for_each(|(interval, dissolve_delay)| {
                let interval_step = dissolve_delay_interval_seconds * interval as u64;
                let randomized_step = dissolve_delay - interval_step;
                assert!(randomized_step <= dissolve_delay_interval_seconds);
            })
    }

    // Test that saturating arithmetic prevents overflow
    #[test]
    fn test_generate_random_dissolve_delay_intervals_with_overflow() {
        let mut rng = ChaCha20Rng::seed_from_u64(1_u64);

        // The edge case values. These are the lowest possible values the
        // algorithm accepts
        let interval_count = 3;
        let dissolve_delay_interval_seconds = u64::MAX;

        let random_dissolve_delay_intervals = generate_random_dissolve_delay_intervals(
            interval_count,
            dissolve_delay_interval_seconds,
            &mut rng,
        );

        // As the dissolve_delay_interval_seconds is u64::MAX the saturating arithmetic should
        // result in dissolve_delays that do not overflow and remain at u64::MAX.
        assert_eq!(random_dissolve_delay_intervals[0], 0);
        assert_eq!(random_dissolve_delay_intervals[1], u64::MAX);
        assert_eq!(random_dissolve_delay_intervals[2], u64::MAX);
    }
}

/// Verifies that the url is within the allowed length, and begins with
/// `http://` or `https://`. In addition, it will return an error in case of a
/// possibly "dangerous" condition, such as the url containing a username or
/// password, or having a port, or not having a domain name.
pub fn validate_proposal_url(
    url: &str,
    min_length: usize,
    max_length: usize,
    field_name: &str,
) -> Result<(), String> {
    // Check that the URL is a sensible length
    if url.len() > max_length {
        return Err(format!(
            "{field_name} must be less than {max_length} characters long, but it is {} characters long. (Field was set to `{url}`.)",
            url.len(),
        ));
    }
    if url.len() < min_length {
        return Err(format!(
            "{field_name} must be greater or equal to than {min_length} characters long, but it is {} characters long. (Field was set to `{url}`.)",
            url.len(),
        ));
    }

    let url = Url::parse(url).map_err(|_| format!("{field_name} must be a valid URL."))?;

    if url.cannot_be_a_base() {
        return Err(format!("{field_name} must be a valid link",));
    }

    let scheme_is_http_or_https = url.scheme() == "http" || url.scheme() == "https";
    if !scheme_is_http_or_https {
        return Err(format!(
            "{field_name} must begin with http:// or https://. (Field was set to `{url}`.)",
        ));
    }

    let has_login = url.username() != "" || url.password().is_some();
    if has_login {
        return Err(format!(
            "{field_name} should not contain a username or password.  (Field was set to `{url}`.)",
        ));
    }

    match url.host() {
        Some(Host::Domain(_)) => {}
        Some(_) => {
            return Err(format!(
                "{field_name} should have a domain name. (It was `{url}`.)",
            ))
        }
        None => {
            return Err(format!(
                "{field_name} should have a host. (It was `{url}`.)",
            ))
        }
    }

    if url.port().is_some() {
        return Err(format!(
            "{field_name} should not contain a port.  (It was `{url}`.)",
        ));
    }

    Ok(())
}

/// Returns the total amount of memory (heap, stable memory, etc) that the calling canister has allocated.
#[cfg(any(target_arch = "wasm32"))]
pub fn total_memory_size_bytes() -> usize {
    core::arch::wasm32::memory_size(0) * WASM_PAGE_SIZE_BYTES
}

#[cfg(not(any(target_arch = "wasm32")))]
pub fn total_memory_size_bytes() -> usize {
    0
}

/// Returns the amount of stable memory that the calling canister has allocated.
#[cfg(any(target_arch = "wasm32"))]
pub fn stable_memory_size_bytes() -> usize {
    dfn_core::api::stable_memory_size_in_pages() as usize * WASM_PAGE_SIZE_BYTES
}

#[cfg(not(any(target_arch = "wasm32")))]
pub fn stable_memory_size_bytes() -> usize {
    0
}
