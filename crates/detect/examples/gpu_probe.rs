// Tiny manual probe — drop in /tmp, build with cargo run --example gpu_probe
fn main() {
    for g in rotten_apple_detect::enumerate_gpus() {
        println!("{:?}", g);
    }
}
