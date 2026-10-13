
            struct Example {
                func open(_ value: [String: Any]) {}
                func run(_ input: [String: Any]) {
                    switch 1 {
                    case 1:
                        guard let params = input["params"] as? [String: Any] else { return }
                        open(params)
                    default: return
                    }
                }
            }
        