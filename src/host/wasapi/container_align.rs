//! Moving samples between CPAL's right-aligned sample types and WASAPI's left-justified
//! containers.
//!
//! A `WAVEFORMATEXTENSIBLE` describes a sample with two numbers: `wBitsPerSample`, the container
//! size, and `wValidBitsPerSample`, how much of it the sample occupies. When they differ, ksmedia.h
//! is normative: the valid bits are left-aligned in the container and the low bits are zero. CPAL's
//! `SampleFormat::I24` is the other way round — a `dasp_sample::I24` sits at the *bottom* of its
//! four-byte container — so the two have to be reconciled at the edge of the backend: up on the way
//! out to the device, down on the way in from it.

/// The container width these functions know how to walk, in bytes. `SampleFormat::I24` — 24 valid
/// bits in four bytes — is the only padded container CPAL negotiates.
const CONTAINER_BYTES: usize = 4;

/// How far a sample must move up to sit left-justified in its container, both counts in bits.
///
/// `Some(0)` means the bytes are already what the device wants: the container is exactly full, or
/// the format declares no valid-bit count. `None` means the container is padded but not one these
/// functions can walk — a format to refuse, since passing it through unshifted would be silently
/// wrong by the width of the padding.
pub(super) fn padding_bits(container_bits: u16, valid_bits: u16) -> Option<u32> {
    if valid_bits == 0 || valid_bits >= container_bits {
        return Some(0);
    }
    if usize::from(container_bits) != CONTAINER_BYTES * 8 {
        return None;
    }
    Some(u32::from(container_bits - valid_bits))
}

/// Moves every container in `buffer` up by `shift` bits, in place: right-aligned → left-justified.
///
/// A trailing partial container, which a correctly sized audio buffer does not have, is left alone.
pub(super) fn left_justify(buffer: &mut [u8], shift: u32) {
    if shift == 0 {
        return;
    }
    debug_assert!(shift < (CONTAINER_BYTES * 8) as u32);
    for container in buffer.chunks_exact_mut(CONTAINER_BYTES) {
        let mut bytes = [0u8; CONTAINER_BYTES];
        bytes.copy_from_slice(container);
        // Shifted as unsigned: the bit pattern is the same either way, and the largest negative
        // sample lands exactly on `i32::MIN`, which is a legal container and not an overflow.
        let justified = u32::from_ne_bytes(bytes) << shift;
        container.copy_from_slice(&justified.to_ne_bytes());
    }
}

/// Copies `src` into `dst`, moving every container down by `shift` bits on the way:
/// left-justified → right-aligned.
///
/// A copy rather than an in-place shift because the source is the buffer WASAPI lends the backend
/// for the duration of a callback, which is not the backend's to write to.
pub(super) fn right_align_into(src: &[u8], dst: &mut [i32], shift: u32) {
    debug_assert!(shift < (CONTAINER_BYTES * 8) as u32);
    debug_assert_eq!(src.len(), dst.len() * CONTAINER_BYTES);
    for (sample, container) in dst.iter_mut().zip(src.chunks_exact(CONTAINER_BYTES)) {
        let mut bytes = [0u8; CONTAINER_BYTES];
        bytes.copy_from_slice(container);
        // Arithmetic shift: the sign has to follow the sample down the container, or every
        // negative sample arrives as a large positive one. The bits shifted off the bottom are
        // exactly the ones the format declares meaningless.
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
        assert_eq!(padding_bits(32, 24), Some(8));
        // A full container needs no shift, whatever its width, and nonsense valid-bit counts
        // declare no padding.
        for (container_bits, valid_bits) in
            [(8, 8), (16, 16), (24, 24), (32, 32), (32, 0), (32, 33)]
        {
            assert_eq!(padding_bits(container_bits, valid_bits), Some(0));
        }
        // Spare bits in a container with no walk behind it: answering zero would put the samples
        // out by the width of the padding with nothing reporting it.
        for (container_bits, valid_bits) in [(16, 12), (24, 20), (64, 48)] {
            assert_eq!(padding_bits(container_bits, valid_bits), None);
        }
    }

    #[test]
    fn samples_move_to_the_top_of_the_container_and_back() {
        let shift = padding_bits(32, 24).unwrap();
        let samples = [I24_MIN, -12_345, -1, 0, 1, 12_345, I24_MAX];

        let mut buffer = to_bytes(&samples);
        left_justify(&mut buffer, shift);
        assert_eq!(
            to_samples(&buffer),
            [
                // The largest negative sample fills the container exactly.
                i32::MIN,
                -12_345 << 8,
                0xFFFF_FF00_u32 as i32,
                0,
                0x0000_0100,
                12_345 << 8,
                0x7FFF_FF00,
            ]
        );

        let mut read_back = vec![0i32; samples.len()];
        right_align_into(&buffer, &mut read_back, shift);
        assert_eq!(read_back, samples);
    }

    #[test]
    fn right_align_carries_the_sign_down_and_drops_the_padding() {
        let shift = padding_bits(32, 24).unwrap();
        // The last container has dirty padding bits, which the format declares meaningless.
        let from_device = to_bytes(&[i32::MIN, 0xFFFF_FF00_u32 as i32, 0, 0x0000_01FF]);

        let mut samples = vec![0i32; from_device.len() / CONTAINER_BYTES];
        right_align_into(&from_device, &mut samples, shift);

        assert_eq!(samples, [I24_MIN, -1, 0, 1]);
        // Every sample is back inside the range `dasp_sample::I24` guarantees.
        assert!(samples.iter().all(|s| (I24_MIN..=I24_MAX).contains(s)));
    }

    #[test]
    fn a_trailing_partial_container_is_left_alone() {
        let shift = padding_bits(32, 24).unwrap();
        let mut buffer = to_bytes(&[1, 2]);
        buffer.extend_from_slice(&[0xAB, 0xCD]);

        left_justify(&mut buffer, shift);

        assert_eq!(to_samples(&buffer[..2 * CONTAINER_BYTES]), [0x100, 0x200]);
        assert_eq!(&buffer[2 * CONTAINER_BYTES..], &[0xAB, 0xCD]);
    }

    #[test]
    fn a_full_container_is_passed_through_untouched() {
        let samples = to_bytes(&[0, 1, -1, i32::MIN, i32::MAX, 12_345]);

        let mut buffer = samples.clone();
        left_justify(&mut buffer, padding_bits(32, 32).unwrap());
        assert_eq!(buffer, samples);

        // And the same on the way in: with no shift the copy is just a copy.
        let mut read_back = vec![0i32; samples.len() / CONTAINER_BYTES];
        right_align_into(&samples, &mut read_back, 0);
        assert_eq!(to_bytes(&read_back), samples);
    }
}
