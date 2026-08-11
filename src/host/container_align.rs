//! Moving samples between CPAL's right-aligned sample types and a wider, left-justified container.
//!
//! A `WAVEFORMATEXTENSIBLE` describes a sample with two numbers: `wBitsPerSample`, the size of the
//! container, and `wValidBitsPerSample`, how much of that container the sample actually occupies.
//! When the two differ the valid bits sit at the *top* of the container and the spare bits at the
//! bottom — "a 20-bit sample can be stored left-justified within a three-byte container", as the
//! WDK's *Extensible Wave-Format Descriptors* puts it.
//!
//! CPAL's sample types are the other way round. [`SampleFormat::I24`] is a `dasp_sample::I24`,
//! which is an `i32` restricted to `-(1 << 23)..=(1 << 23) - 1`: the sample sits at the *bottom*
//! of its four-byte container. Handing those bytes straight to a device that asked for 24-in-32
//! makes every sample 2^8 too small, and reading that device's bytes as if they were already
//! right-aligned makes every sample 2^8 too large. Both were measured on a PreSonus AudioBox
//! 22VSL at 24-in-32 / 48 kHz: output 48 dB too quiet, input pinned to full scale by anything
//! above near-silence.
//!
//! So the conversion happens at the edge of the backend — up on the way out to the device, down
//! on the way in from it. Formats whose container is exactly full (`I16`, `I32`, `F32`, …) get a
//! shift of zero from [`padding_bits`] and are not touched at all.
//!
//! [`SampleFormat::I24`]: crate::SampleFormat::I24

/// The container width these functions know how to walk, in bytes.
///
/// The only padded container CPAL negotiates is `SampleFormat::I24` — 24 valid bits in four
/// bytes — so this is the one width worth handling; [`padding_bits`] answers zero for every
/// other, which leaves the buffer alone.
const CONTAINER_BYTES: usize = 4;

/// How far a sample must move up to sit left-justified in its container, given the container size
/// and the valid-bit count of the negotiated format, both in bits.
///
/// Returns zero — meaning "these bytes are already what the device wants" — whenever the
/// container is exactly full, the format declares no valid-bit count, or the container is not one
/// [`left_justify`] and [`right_align_into`] can walk.
pub(crate) fn padding_bits(container_bits: u16, valid_bits: u16) -> u32 {
    if container_bits as usize != CONTAINER_BYTES * 8 {
        return 0;
    }
    if valid_bits == 0 || valid_bits >= container_bits {
        return 0;
    }
    u32::from(container_bits - valid_bits)
}

/// Moves every container in `buffer` up by `shift` bits, in place: right-aligned → left-justified.
///
/// A trailing partial container, which a correctly sized audio buffer does not have, is left
/// alone.
pub(crate) fn left_justify(buffer: &mut [u8], shift: u32) {
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
/// A copy rather than an in-place shift because the source is the buffer WASAPI lends the
/// backend for the duration of a callback, which is not the backend's to write to.
pub(crate) fn right_align_into(src: &[u8], dst: &mut [i32], shift: u32) {
    debug_assert!(shift < (CONTAINER_BYTES * 8) as u32);
    debug_assert_eq!(src.len(), dst.len() * CONTAINER_BYTES);
    for (sample, container) in dst.iter_mut().zip(src.chunks_exact(CONTAINER_BYTES)) {
        let mut bytes = [0u8; CONTAINER_BYTES];
        bytes.copy_from_slice(container);
        // Arithmetic shift: the sign has to follow the sample down the container, or every
        // negative sample arrives as a large positive one. The padding bits shifted off the
        // bottom are exactly the ones the format declares meaningless.
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
        // Any other partly-used 32-bit container follows the same arithmetic.
        assert_eq!(padding_bits(32, 20), 12);
    }

    #[test]
    fn a_full_container_needs_no_shift() {
        assert_eq!(padding_bits(8, 8), 0);
        assert_eq!(padding_bits(16, 16), 0);
        assert_eq!(padding_bits(32, 32), 0);
        assert_eq!(padding_bits(64, 64), 0);
        // Packed 24-bit: three-byte container, nothing spare in it.
        assert_eq!(padding_bits(24, 24), 0);
    }

    #[test]
    fn a_container_that_cannot_be_walked_needs_no_shift() {
        // Widths with no walk behind them, however much of them is spare.
        assert_eq!(padding_bits(64, 48), 0);
        assert_eq!(padding_bits(24, 20), 0);
        // Nonsense the format could still contain.
        assert_eq!(padding_bits(32, 0), 0);
        assert_eq!(padding_bits(32, 33), 0);
    }

    #[test]
    fn a_sample_survives_the_round_trip_through_a_wider_container() {
        let shift = padding_bits(32, 24);
        let samples = [I24_MIN, -8_000_000, -12_345, -1, 0, 1, 12_345, I24_MAX];

        let mut buffer = to_bytes(&samples);
        left_justify(&mut buffer, shift);

        let mut read_back = vec![0i32; samples.len()];
        right_align_into(&buffer, &mut read_back, shift);

        assert_eq!(read_back, samples);
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
        let shift = padding_bits(32, 24);
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
        right_align_into(&from_device, &mut samples, shift);

        assert_eq!(samples, [0, 1, -1, I24_MAX, I24_MIN, 1]);
        // Every sample is back inside the range `dasp_sample::I24` guarantees.
        assert!(samples.iter().all(|s| (I24_MIN..=I24_MAX).contains(s)));
    }

    #[test]
    fn a_sixteen_bit_format_is_passed_through_untouched() {
        let shift = padding_bits(16, 16);
        let samples: Vec<u8> = [0i16, 1, -1, i16::MIN, i16::MAX, 12_345]
            .iter()
            .flat_map(|s| s.to_ne_bytes())
            .collect();

        let mut buffer = samples.clone();
        left_justify(&mut buffer, shift);

        assert_eq!(buffer, samples);
    }

    #[test]
    fn a_thirty_two_bit_format_is_passed_through_untouched() {
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
