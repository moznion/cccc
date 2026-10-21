
            int &get(int x) {
                if (x) { return get(0); }
                return g;
            }
            int &&take(int x) {
                if (x) { }
                return 0;
            }
        