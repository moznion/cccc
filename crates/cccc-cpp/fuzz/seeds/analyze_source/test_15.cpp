
            template <int N>
            int fact() {
                if (N) { return N * fact<N - 1>(); }
                return 1;
            }
        