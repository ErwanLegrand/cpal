//! Moving samples between CPAL's right-aligned sample types and the left-justified container a
//! `WAVEFORMATEXTENSIBLE` describes.
//!
//! Where `wValidBitsPerSample` is less than `wBitsPerSample`, ksmedia.h puts the valid bits at the
//! *top* of the container and zeroes the rest, while CPAL's [`SampleFormat::I24`] keeps its sample
//! at the *bottom* of an `i32`. A backend negotiating such a format shifts up on the way out and
//! down on the way in. The convention is the container format's rather than any one API's, so the
//! arithmetic lives here instead of inside a host.
//!
//! [`SampleFormat::I24`]: crate::SampleFormat::I24

/// The container width these functions walk, in bytes.
const CONTAINER_BYTES: usize = 4;

/// How far a sample must move up to sit left-justified in its container, given the container size
/// and the valid-bit count of the negotiated format, both in bits.
///
/// Zero means the bytes are already what the device wants, and neither [`left_justify`] nor
/// [`right_align_into`] then touches them at all.
///
/// # Panics
///
/// [`left_justify`] and [`right_align_into`] step through [`CONTAINER_BYTES`] at a time, so a
/// padded container of any other width would be shifted as if it were four bytes wide. Debug
/// builds assert against that, and against more valid bits than the container holds; release
/// builds answer as if the container were full, leaving the bytes untouched rather than mangled.
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
    u32::from(container_bits - valid_bits)
}

/// Moves every container in `buffer` up by `shift` bits, in place: right-aligned → left-justified.
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

/// Copies `src` into `dst`, moving every container down by `shift` bits on the way:
/// left-justified → right-aligned.
///
/// A copy rather than an in-place shift because the source is the buffer WASAPI lends the
/// backend for the duration of a callback, which is not the backend's to write to.
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

    /// The smallest and largest samples `SampleFormat::I24` can hold.
    const I24_MIN: i32 = -(1 << 23);
    const I24_MAX: i32 = (1 << 23) - 1;

    fn to_bytes(samples: &[i32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_ne_bytes()).collect()
    }

    fn to_samples(bytes: &[u8]) -> Vec<i32> {
        let mut samples = vec![0i32; bytes.len() / CONTAINER_BYTES];
        right_align_into(bytes, &mut samples, 0);
        samples
    }

    #[test]
    fn padding_bits_is_the_gap_between_the_container_and_the_sample() {
        // 24-in-32, the one case CPAL actually negotiates.
        assert_eq!(padding_bits(32, 24), 8);
        assert_eq!(padding_bits(32, 20), 12);
    }

    #[test]
    fn a_full_container_needs_no_shift() {
        // Answered before the container width is looked at, so every width reaches this.
        assert_eq!(padding_bits(8, 8), 0);
        assert_eq!(padding_bits(16, 16), 0);
        // Packed 24-bit: three-byte container, nothing spare in it.
        assert_eq!(padding_bits(24, 24), 0);
        assert_eq!(padding_bits(32, 32), 0);
        assert_eq!(padding_bits(64, 64), 0);
        // A format declaring no valid-bit count at all: the container is full by definition.
        assert_eq!(padding_bits(32, 0), 0);
    }

    /// Nothing CPAL negotiates reaches here — `config_to_waveformatextensible` pads only `I24`, in
    /// four bytes — so this stands in for a format a later change might add.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic = "not one this module can walk"]
    fn a_padded_container_of_another_width_trips_the_invariant() {
        // 12-in-16: spare bits, but `left_justify` would step through it four bytes at a time.
        let _ = padding_bits(16, 12);
    }

    #[test]
    fn left_justify_puts_the_sample_at_the_top_of_the_container() {
        let shift = padding_bits(32, 24);
        let mut buffer = to_bytes(&[0, 1, -1, I24_MAX, I24_MIN]);
        left_justify(&mut buffer, shift);

        assert_eq!(
            to_samples(&buffer),
            [
                0,
                0x0000_0100,
                0xFFFF_FF00_u32 as i32,
                0x7FFF_FF00,
                // The largest negative sample fills the container exactly.
                i32::MIN,
            ]
        );
    }

    #[test]
    fn right_align_carries_the_sign_down_and_drops_the_padding() {
        // What a device hands over: samples at the top of the container. The last one has dirty
        // padding bits, which the format declares meaningless and this must discard.
        let from_device = to_bytes(&[
            0,
            0x0000_0100,
            0xFFFF_FF00_u32 as i32,
            0x7FFF_FF00,
            i32::MIN,
            0x0000_01FF,
        ]);

        let mut samples = vec![0i32; from_device.len() / CONTAINER_BYTES];
        right_align_into(&from_device, &mut samples, padding_bits(32, 24));

        assert_eq!(samples, [0, 1, -1, I24_MAX, I24_MIN, 1]);
        // Every sample is back inside the range `dasp_sample::I24` guarantees.
        assert!(samples.iter().all(|s| (I24_MIN..=I24_MAX).contains(s)));
    }

    #[test]
    fn a_trailing_partial_container_is_left_alone() {
        let shift = padding_bits(32, 24);
        let mut buffer = to_bytes(&[1, 2]);
        buffer.extend_from_slice(&[0xAB, 0xCD]);

        left_justify(&mut buffer, shift);

        assert_eq!(to_samples(&buffer[..2 * CONTAINER_BYTES]), [0x100, 0x200]);
        assert_eq!(&buffer[2 * CONTAINER_BYTES..], &[0xAB, 0xCD]);
    }

    #[test]
    fn a_full_container_format_is_passed_through_untouched() {
        let shift = padding_bits(32, 32);
        let samples = to_bytes(&[0, 1, -1, i32::MIN, i32::MAX, 12_345]);

        let mut buffer = samples.clone();
        left_justify(&mut buffer, shift);
        assert_eq!(buffer, samples);

        // And the same on the way in: with no shift the copy is just a copy.
        let mut read_back = vec![0i32; samples.len() / CONTAINER_BYTES];
        right_align_into(&samples, &mut read_back, shift);
        assert_eq!(to_bytes(&read_back), samples);
    }
}
