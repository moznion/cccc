
            func f(
                value: Value?,
                values: [Int]?,
                callback: (() -> Void)?
            ) {
                _ = value?.property
                _ = value?.child?.run()
                _ = values?[0]
                callback?()
            }
        