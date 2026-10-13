
            func f(x: Int?) {
                #if DEBUG
                if x == nil {
                    log()
                }
                #endif
            }
        