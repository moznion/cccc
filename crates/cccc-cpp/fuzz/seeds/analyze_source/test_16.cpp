
            struct S {
                template <typename T>
                void walk(int n) {
                    if (n) { this->walk<T>(n - 1); }
                }
                template <typename T>
                void visit(S &s, int n) {
                    if (n) { s.template visit<T>(s, n - 1); }
                }
            };
        