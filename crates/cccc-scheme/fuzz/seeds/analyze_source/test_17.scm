).unwrap();
        assert_eq!(inner.line, 2);
    }

    #[test]
    fn define_with_lambda_value_borrows_the_name() {
        let src = r#