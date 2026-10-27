//! Plain wasm cannot spawn Rayon threads. Run the same kernels serially on that target.
#[cfg(not(target_arch = "wasm32"))]
pub use rayon::prelude::*;
#[cfg(target_arch = "wasm32")]
pub trait Slices<T> {
    fn par_iter(&self) -> std::slice::Iter<'_, T>;
    fn par_iter_mut(&mut self) -> std::slice::IterMut<'_, T>;
}
#[cfg(target_arch = "wasm32")]
impl<T> Slices<T> for [T] {
    fn par_iter(&self) -> std::slice::Iter<'_, T> {
        self.iter()
    }
    fn par_iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
        self.iter_mut()
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub fn reduce<T: Send>(
    iter: impl ParallelIterator<Item = T>,
    identity: impl Fn() -> T + Sync + Send,
    combine: impl Fn(T, T) -> T + Sync + Send,
) -> T {
    iter.reduce(identity, combine)
}
#[cfg(target_arch = "wasm32")]
pub fn reduce<T>(
    iter: impl Iterator<Item = T>,
    identity: impl Fn() -> T,
    combine: impl Fn(T, T) -> T,
) -> T {
    iter.fold(identity(), combine)
}
