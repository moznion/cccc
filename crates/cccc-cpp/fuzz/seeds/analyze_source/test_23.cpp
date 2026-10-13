
            template <typename T>
            class PeriodicTaskBase {
            public:
                void run(T ticks) {
                    if (ticks) { }
                }
            };

            template class PeriodicTaskBase<uint16_t>;
            