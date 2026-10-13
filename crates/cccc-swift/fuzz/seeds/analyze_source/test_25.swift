
            struct S {
                let xs: [Int]
                subscript(i: Int) -> Int {
                    if i < 0 {
                        return 0
                    }
                    return xs[i]
                }
            }
        