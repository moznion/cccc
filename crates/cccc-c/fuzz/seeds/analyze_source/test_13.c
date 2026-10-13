
            void f(void (*g)(void)) {
                if (g) {
                    (*g)();
                }
            }
        