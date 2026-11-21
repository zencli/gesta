use std::time::Instant;

fn main() {
    // The sort the fix proposes, on the real shape: 100,003 relative paths.
    let mut paths: Vec<(String, usize)> = (0..100003)
        .map(|i| (format!("src/dir{}/file{}.txt", i % 200, i), i))
        .collect();
    // Mixed order, as a parallel walk hands them back.
    paths.reverse();
    paths.swap(0, 50000);

    let t = Instant::now();
    paths.sort_by(|a, b| a.0.cmp(&b.0));
    println!("sort_by rel: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);

    // Unsorted as a baseline for the same loop shape.
    let mut again: Vec<(String, usize)> = (0..100003)
        .map(|i| (format!("src/dir{}/file{}.txt", i % 200, i), i))
        .collect();
    again.reverse();
    let t = Instant::now();
    let mut n = 0usize;
    for p in &again {
        n += p.0.len();
    }
    println!("scan only:   {:.1} ms ({})", t.elapsed().as_secs_f64() * 1000.0, n);
}
