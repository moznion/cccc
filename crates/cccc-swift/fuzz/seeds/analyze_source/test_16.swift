
            func f(a: Bool) {
                outer: while a {
                    break outer
                }
                while a {
                    break
                }
            }
        