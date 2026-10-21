
            class C {
                String getWords(int number) {
                    return switch (number) {
                        case 1 -> "one";
                        case 2 -> "a couple";
                        default -> "lots";
                    };
                }
            }
        