//! Standalone CLI runner for ProcessKit containment and lifecycle diagnostics.

fn main() {
    eprintln!("processkit-cli is not implemented yet. See README.md and docs/ROADMAP.md.");
}

#[cfg(test)]
mod tests {
    #[test]
    fn binary_name_is_stable() {
        assert_eq!(env!("CARGO_PKG_NAME"), "processkit-cli");
    }
}
