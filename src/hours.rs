#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoakHours(u32);

impl SoakHours {
    pub const DEFAULT: Self = Self(24);

    pub fn new(n: u32) -> Option<Self> {
        (n >= 1).then_some(Self(n))
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero() {
        assert_eq!(SoakHours::new(0), None);
    }

    #[test]
    fn accepts_one() {
        assert_eq!(SoakHours::new(1).map(|h| h.get()), Some(1));
    }

    #[test]
    fn default_is_24() {
        assert_eq!(SoakHours::DEFAULT.get(), 24);
    }
}
