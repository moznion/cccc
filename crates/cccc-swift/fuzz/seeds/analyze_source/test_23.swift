
            class C {
                var name = ""
                var flag: Int {
                    if name.isEmpty {
                        return 0
                    }
                    return 1
                }
            }
        