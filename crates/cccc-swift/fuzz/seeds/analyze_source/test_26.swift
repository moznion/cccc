
            func f(x: Int?, a: Bool, b: Bool) {
                if let v = x, a && b {
                    use(v)
                }
            }
        