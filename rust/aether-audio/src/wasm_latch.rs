/// Sticky post-decode render error for one WASM owner.
#[derive(Default)]
struct RenderFailureLatch(Option<u32>);
impl RenderFailureLatch {
    fn status(&self) -> Option<u32> {
        self.0
    }
    fn record(&mut self, status: u32) {
        if self.0.is_none() {
            self.0 = Some(status);
        }
    }
    fn reset(&mut self) {
        self.0 = None;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_failure_stays_until_successful_owner_reset_and_is_independent() {
        let mut a = RenderFailureLatch::default();
        let b = RenderFailureLatch::default();
        assert_eq!(a.status(), None);
        a.record(205);
        a.record(210);
        assert_eq!(a.status(), Some(205));
        assert_eq!(b.status(), None);
        a.reset();
        assert_eq!(a.status(), None);
    }
}
