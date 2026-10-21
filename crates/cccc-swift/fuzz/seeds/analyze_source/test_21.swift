
            class C {
                var x: Int
                init(x: Int, big: Bool) {
                    if big {
                        self.x = x * 2
                    } else {
                        self.x = x
                    }
                }
                deinit {
                    if x > 0 {
                        log()
                    }
                }
            }
        