
            func f(s: String?, a: Bool) -> Int {
                guard let v = s else {
                    if a {
                        return -1
                    }
                    return 0
                }
                return v.count
            }
        