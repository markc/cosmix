pub fn observed_answer() -> u8 {
    renamed_leaf::answer()
}

#[cfg(test)]
mod tests {
    #[test]
    fn deliberately_broken_reverse_dependency_test() {
        assert_eq!(super::observed_answer(), 41, "reverse dependency must run");
    }
}
