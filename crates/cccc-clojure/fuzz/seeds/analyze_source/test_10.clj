
            (defn host [xs]
              (letfn [(helper [x] (if x 1 0))]
                (map helper xs)))
        