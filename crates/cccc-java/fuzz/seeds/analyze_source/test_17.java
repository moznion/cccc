
            class C {
                int walk(int n) {
                    if (n == 0) {
                        return 0;
                    }
                    return this.walk(n - 1);
                }
            }
        