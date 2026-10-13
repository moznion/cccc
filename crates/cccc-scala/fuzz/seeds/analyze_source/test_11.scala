
            object S {
                def f(t: (Int, Int)): Int = t match {
                    case (a, b) => if (a > b) a else b
                }
            }
        