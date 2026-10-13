
            int process(int n, std::vector<int> &items) {
                switch (n % 3) {
                    case 0:
                        for (auto &x : items) {
                            auto scored = [](int v) {
                                return v > 0 && v < 10;
                            };
                            if (scored(x)) {
                                break;
                            }
                        }
                        break;
                    case 1:
                        return process(n - 1, items);
                    default:
                        goto done;
                }
            #ifdef DEBUG
                log_debug(n);
            #endif
            done:
                return n;
            }
        