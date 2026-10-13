
            object S {
                def f(xs: List[(Int, Int)]): List[Int] = xs.map {
                    case (0, v) => v
                    case (k, v) => k
                }
            }
        