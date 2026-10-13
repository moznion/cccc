
            func f(g: () async throws -> Int, a: Bool) async {
                if a {
                    let r = try? await g()
                    print(r ?? -1)
                }
            }
        