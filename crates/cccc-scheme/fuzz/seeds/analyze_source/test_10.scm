
            (define (safe thunk)
              (guard (e ((error-object? e) 'err))
                (thunk)))
        