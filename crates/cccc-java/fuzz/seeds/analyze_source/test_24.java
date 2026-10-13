);
        assert_eq!(ctor.cognitive, 1);
    }

    #[test]
    fn pattern_switch_with_guard_parses_and_scores() {
        let src = r#