
            object S {
                def f(m: Map[String, Int]): List[String] = m.toList.map {
                    case (k, v) => if (v > 0) k else ""
                }
            }
        