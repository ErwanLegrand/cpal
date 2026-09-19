//! Moving samples between CPAL's right-aligned sample types and the left-justified container a
//! `WAVEFORMATEXTENSIBLE` describes.
//!
//! Where `wValidBitsPerSample` is less than `wBitsPerSample`, ksmedia.h puts the valid bits at the
//! *top* of the container and zeroes the rest, while CPAL's [`SampleFormat::I24`] keeps its sample
//! at the *bottom* of an `i32`. WASAPI negotiates such formats, so this backend shifts samples up
//! on the way out and down on the way in.
//!
//! [`SampleFormat::I24`]: crate::SampleFormat::I24

// The container width these functions walk, in bytes.
const CONTAINER_BYTES: usize = 4;

// How far a sample must move up to sit left-justified in its container, given the container size
// and the valid-bit count of the negotiated format, both in bits.
//
// Zero means the bytes are already what the device wants, and neither `left_justify` nor
// `right_align_into` then touches them at all.
//
// `left_justify` and `right_align_into` step through `CONTAINER_BYTES` at a time, so a padded
// container of any other width would be shifted as if it were four bytes wide. Debug builds assert
// against that, and against more valid bits than the container holds; release builds answer as if
// the container were full, leaving the bytes untouched rather than mangled.
pub(crate) fn padding_bits(container_bits: u16, valid_bits: u16) -> u32 {
    debug_assert!(
        valid_bits <= container_bits,
        "{valid_bits} valid bits do not fit in a {container_bits}-bit container",
    );
    if valid_bits == 0 || valid_bits >= container_bits {
        return 0;
    }
    debug_assert_eq!(
        container_bits as usize,
        CONTAINER_BYTES * 8,
        "a padded {container_bits}-bit container is not one this module can walk",
    );
    if container_bits as usize != CONTAINER_BYTES * 8 {
        return 0;
    }
    u32::from(container_bits - valid_bits)
}

// Moves every container in `buffer` up by `shift` bits, in place: right-aligned → left-justified.
pub(crate) fn left_justify(buffer: &mut [u8], shift: u32) {
    if shift == 0 {
        return;
    }
    debug_assert!(shift < (CONTAINER_BYTES * 8) as u32);
    for container in buffer.chunks_exact_mut(CONTAINER_BYTES) {
        let mut bytes = [0u8; CONTAINER_BYTES];
        bytes.copy_from_slice(container);
        let justified = u32::from_ne_bytes(bytes) << shift;
        container.copy_from_slice(&justified.to_ne_bytes());
    }
}

// Copies `src` into `dst`, moving every container down by `shift` bits on the way:
// left-justified → right-aligned.
//
// A copy rather than an in-place shift because the source is the buffer WASAPI lends the
// backend for the duration of a callback, which is not the backend's to write to.
pub(crate) fn right_align_into(src: &[u8], dst: &mut [i32], shift: u32) {
    debug_assert!(shift < (CONTAINER_BYTES * 8) as u32);
    debug_assert_eq!(src.len(), dst.len() * CONTAINER_BYTES);
    for (sample, container) in dst.iter_mut().zip(src.chunks_exact(CONTAINER_BYTES)) {
        let mut bytes = [0u8; CONTAINER_BYTES];
        bytes.copy_from_slice(container);
        *sample = i32::from_ne_bytes(bytes) >> shift;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const I24_MIN: i32 = -(1 << 23);
    const I24_MAX: i32 = (1 << 23) - 1;

    #[test]
    fn padding_bits_is_the_gap_between_the_container_and_the_sample() {
        // 24-in-32, the one case CPAL actually negotiates.
        assert_eq!(padding_bits(32, 24), 8);
        assert_eq!(padding_bits(32, 20), 12);
    }

    #[test]
    fn left_justify_moves_i24_up_eight_bits() {
        // Literal right-aligned I24 containers (1, -1, I24_MAX, I24_MIN), then each shifted up
        // eight bits toward the top of its four-byte container.
        let mut buffer = vec![
            1u8, 0u8, 0u8, 0u8, 0xFFu8, 0xFFu8, 0xFFu8, 0xFFu8, 0xFFu8, 0xFFu8, 0x7Fu8,
            0u8, // I24_MAX
            0u8, 0u8, 0x80u8, 0xFFu8, // I24_MIN
        ];
        left_justify(&mut buffer, 8);
        assert_eq!(
            buffer,
            vec![
                0u8, 1u8, 0u8, 0u8, 0u8, 0xFFu8, 0xFFu8, 0xFFu8, 0u8, 0xFFu8, 0xFFu8, 0x7Fu8, 0u8,
                0u8, 0u8, 0x80u8,
            ]
        );
    }

    #[test]
    fn right_align_into_drops_the_padding_and_carries_the_sign() {
        // Samples at the top of the container as a device hands them over; the last one has dirty
        // padding bits, which the format declares meaningless and this must discard.
        let from_device = vec![
            0u8, 1u8, 0u8, 0u8, 0u8, 0xFFu8, 0xFFu8, 0xFFu8, 0u8, 0xFFu8, 0xFFu8, 0x7Fu8, 0u8, 0u8,
            0u8, 0x80u8, 0xFFu8, 1u8, 0u8, 0u8, // dirty padding: still 1
        ];
        let mut samples = vec![0i32; 5];
        right_align_into(&from_device, &mut samples, 8);
        assert_eq!(samples, [1, -1, I24_MAX, I24_MIN, 1]);
        assert!(samples.iter().all(|s| (I24_MIN..=I24_MAX).contains(s)));
    }

    #[test]
    fn a_trailing_partial_container_is_left_alone() {
        let mut buffer = vec![1u8, 0u8, 0u8, 0u8, 2u8, 0u8, 0u8, 0u8, 0xABu8, 0xCDu8];
        left_justify(&mut buffer, 8);
        assert_eq!(
            buffer,
            vec![0u8, 1u8, 0u8, 0u8, 0u8, 2u8, 0u8, 0u8, 0xABu8, 0xCDu8]
        );
    }

    #[test]
    fn a_full_container_format_is_passed_through_untouched() {
        // Shift zero: the bytes pass through as-is in both directions.
        let shift = padding_bits(32, 32);
        let samples = vec![0u8, 1u8, 0u8, 0u8, 0xFFu8, 0xFFu8, 0xFFu8, 0xFFu8];
        let mut buffer = samples.clone();
        left_justify(&mut buffer, shift);
        assert_eq!(buffer, samples);
        let mut read_back = vec![0i32; 2];
        right_align_into(&samples, &mut read_back, shift);
        assert_eq!(read_back, [0x100, -1]);
    }

    // In debug builds the width guard is a debug_assert; in release it is an explicit
    // `return 0` (the runtime defence the release path relies on), which only a release
    // test can see.
    #[test]
    #[cfg(not(debug_assertions))]
    fn an_unwalkable_container_width_answers_zero_in_release() {
        // 12-in-16: spare bits, but `left_justify` would step through it four bytes at a time.
        assert_eq!(padding_bits(16, 12), 0);
    }
}
