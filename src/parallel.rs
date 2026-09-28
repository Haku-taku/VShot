// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! Row-parallel passes for the per-pixel work a capture does.
//!
//! A ten-bit full-screen frame is 3.7 million pixels, and every one of them
//! goes through a transfer curve and a colour matrix on the way in or out.  That
//! is where a capture's time actually goes — measured single-threaded, decoding
//! one such frame takes about 110 ms and encoding one to PQ about 130 ms, on a
//! path the user is waiting on.  These passes are row-independent, so splitting
//! the rows across the machine's cores is the cheapest way to make a capture
//! quick and it changes no output value at all.
//!
//! Small buffers are left alone: starting threads costs more than the work they
//! would share, and most of the region captures in the tests are a few pixels.

/// The fewest elements worth splitting: about a megapixel of bytes, or a
/// quarter of one of RGBA floats.  Below this a pass runs on the calling thread.
const PARALLEL_MIN_ELEMENTS: usize = 1 << 18;

/// How many whole rows one worker takes.  Whole rows rather than elements, so a
/// pass whose unit is a row — a Radiance scanline, say — still fits.
fn rows_per_worker(rows: usize, workers: usize) -> usize {
    rows.div_ceil(workers.max(1)).max(1)
}

fn workers() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

/// Runs `work` over disjoint row-chunks of `out` in parallel.
///
/// `out` is one flat buffer of `width`-element rows.  `work` is handed the index
/// of its chunk's first element and the chunk itself, so a pass can reach into a
/// second buffer — the source it is decoding, say — by that same offset.
pub(crate) fn map_rows<T, F>(out: &mut [T], width: usize, work: F)
where
    T: Send,
    F: Fn(usize, &mut [T]) + Send + Sync,
{
    let total = out.len();
    if width == 0 || total == 0 {
        return;
    }
    let workers = workers();
    let chunk = rows_per_worker(total.div_ceil(width), workers) * width;
    if workers == 1 || chunk >= total || total < PARALLEL_MIN_ELEMENTS {
        work(0, out);
        return;
    }
    let work = &work;
    std::thread::scope(|scope| {
        for (index, piece) in out.chunks_mut(chunk).enumerate() {
            scope.spawn(move || work(index * chunk, piece));
        }
    });
}

/// The same, for a pass whose chunks produce a buffer of their own: a run-length
/// encoded Radiance scanline is not a fixed number of bytes.  The pieces come
/// back in row order.
pub(crate) fn collect_rows<T, R, F>(input: &[T], width: usize, work: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&[T]) -> R + Send + Sync,
{
    let total = input.len();
    if width == 0 || total == 0 {
        return Vec::new();
    }
    let workers = workers();
    let chunk = rows_per_worker(total.div_ceil(width), workers) * width;
    if workers == 1 || chunk >= total || total < PARALLEL_MIN_ELEMENTS {
        return vec![work(input)];
    }
    let work = &work;
    let mut slots: Vec<Option<R>> = input.chunks(chunk).map(|_| None).collect();
    std::thread::scope(|scope| {
        for (slot, piece) in slots.iter_mut().zip(input.chunks(chunk)) {
            scope.spawn(move || *slot = Some(work(piece)));
        }
    });
    slots.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_element_is_visited_exactly_once_whatever_the_split() {
        // The chunks have to cover the buffer once, in order, for a pass to be
        // able to write each element from its own chunk — that is the whole
        // reason the split is safe.
        for width in [1usize, 3, 16, 1000] {
            for rows in [0usize, 1, 5, 4096] {
                let mut out = vec![0usize; width * rows];
                map_rows(&mut out, width, |offset, chunk| {
                    for (index, slot) in chunk.iter_mut().enumerate() {
                        *slot = offset + index + 1;
                    }
                });
                let expected: Vec<usize> = (1..=width * rows).collect();
                assert_eq!(out, expected, "width {width}, rows {rows}");
            }
        }
    }

    #[test]
    fn collected_chunks_come_back_in_row_order() {
        let input: Vec<usize> = (0..4096 * 4).collect();
        let rows = collect_rows(&input, 4, |chunk| chunk.to_vec());
        assert_eq!(rows.concat(), input);
    }
}
