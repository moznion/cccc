
            func f(s: String?) -> Int {
                guard let v = s else {
                    return 0
                }
                return v.count
            }
        