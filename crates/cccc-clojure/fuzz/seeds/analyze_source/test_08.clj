
            (defn safe [thunk]
              (try
                (thunk)
                (catch Exception e
                  (if (recoverable? e) (retry) (abort)))
                (finally (cleanup))))
        