//! Numeric slice preconditions only. No address is dereferenced or authenticated.
pub(super) fn valid_input_range(address: usize, bytes: usize, header: usize, max: usize) -> bool {
    address != 0
        && address.is_multiple_of(8)
        && bytes >= header
        && bytes <= max
        && bytes <= isize::MAX as usize
        && address
            .checked_add(bytes)
            .is_some_and(|end| end <= isize::MAX as usize)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mapped_entry_bounds_accept_complete_header_and_cap_without_owner_claim() {
        // Literal candidate scalar geometry; production passes actual linked R
        // constants. These cases prove range arithmetic only, not that geometry.
        let address = 0x6000_1000_0000;
        assert!(valid_input_range(address, 960, 960, 1024 * 1024));
        assert!(valid_input_range(address, 1024 * 1024, 960, 1024 * 1024));
    }
    #[test]
    fn mapped_entry_bounds_refuse_null_alignment_short_excess_and_wrap() {
        let address = 0x6000_1000_0000;
        for (p, n) in [
            (0, 960),
            (address + 1, 960),
            (address, 0),
            (address, 959),
            (address, 1024 * 1024 + 1),
            (usize::MAX - 7, 960),
            ((isize::MAX as usize) & !7, 960),
            (address, usize::MAX),
        ] {
            assert!(!valid_input_range(p, n, 960, 1024 * 1024));
        }
    }
}
