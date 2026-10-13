
            package p
            func host() {
                pick := func(a, b bool) int {
                    if a && b {
                        return 1
                    } else {
                        return 0
                    }
                }
                _ = pick
            }
        