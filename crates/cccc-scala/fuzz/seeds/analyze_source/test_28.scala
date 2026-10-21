);
        assert_eq!(ctor.cognitive, 0);
        // no unit is named `this`
        assert!(find(&report.functions, 