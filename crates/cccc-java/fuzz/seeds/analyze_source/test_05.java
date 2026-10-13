
            class C {
                C(String @A ... xs) {
                    if (xs == null) {
                        return;
                    }
                }

                Object o = new Object() {
                    void anon(String @A ... xs) {
                        if (xs == null) {
                            return;
                        }
                    }
                };

                F lam = (String @A ... xs) -> xs == null ? 0 : xs.length;
            }

            interface I {
                // pgjdbc's QueryExecutor.createQuery shape
                void abs(int a, String @Nullable ... columnNames);

                default void dflt(String @A ... xs) {
                    if (xs == null) {
                        return;
                    }
                }
            }

            record R(String @A ... xs) {
                R {
                    if (xs == null) {
                        throw new IllegalArgumentException();
                    }
                }
            }

            enum E {
                X;

                void inEnum(String @A ... xs) {
                    if (xs == null) {
                        return;
                    }
                }
            }
        