
            class C {
                void f(int n) {
                    for (int i = 0; i < n; i++) {
                        if (i == 1) {
                            continue;
                        }
                        break;
                    }
                }
            }
        