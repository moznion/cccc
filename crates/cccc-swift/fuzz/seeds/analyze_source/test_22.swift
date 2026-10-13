
            class C {
                var name = ""
                var greeting: String {
                    get {
                        if name.isEmpty {
                            return "hello"
                        }
                        return "hello there"
                    }
                    set {
                        if newValue.isEmpty {
                            return
                        }
                        name = newValue
                    }
                }
            }
        