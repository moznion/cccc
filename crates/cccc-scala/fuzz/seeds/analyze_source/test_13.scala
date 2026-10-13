
            object S {
                def f(n: Int): Int = n match {
                    case 0 => 0
                    case v @ _ => v
                }
            }
        