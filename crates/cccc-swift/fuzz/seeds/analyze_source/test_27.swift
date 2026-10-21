
            enum E { case a, b }
            func f(e: E) {
                if case .a = e {
                    log()
                }
            }
        