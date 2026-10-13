
            class C {
                void f(boolean a, int[] items) {
                    while (a) { }
                    for (int i = 0; i < 3; i++) { }
                    for (int x : items) { }
                    do { } while (a);
                }
            }
        