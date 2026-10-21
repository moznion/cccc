
            (defn f [xs]
              (doseq [x xs]
                (when (pred x) (process x))))
        